"""The Rust core of rho, as Python sees it ([ARCH-agent-host]).

This module is built into the workset process. The `rho` package wraps it;
policy code imports `rho`, not `_rho`. Every function runs in the workset
process and acts on its own workset only. Agent ids are their encoded text
(for example "x4cvbixbtifu"); a chat address is "user" or an agent id.

Blocking calls release the GIL. Calls marked async return awaitables that
run on the asyncio loop of the caller's thread.
"""

from typing import Literal, TypedDict

Role = Literal["user", "tool"]

class NativeRuntime(TypedDict):
    kind: Literal["native"]
    model: Literal["sol", "terra", "astra"]
    effort: Literal["low", "medium", "high", "xhigh"]
    fast: bool

class ClaudeRuntime(TypedDict):
    kind: Literal["claude"]
    model: Literal["fable", "opus", "sonnet", "haiku"]
    effort: Literal["medium", "high", "xhigh"]

Runtime = NativeRuntime | ClaudeRuntime

class Spec(TypedDict):
    runtime: Runtime
    props: dict[str, str]

MentionKind = Literal["ask", "result", "fyi"]

class MessageRecord(TypedDict):
    id: int
    at: int  # unix ms
    topic: int
    sender: str  # "user" or an agent id
    mentions: list[tuple[str, MentionKind]]  # (address, kind)
    text: str

class TopicRecord(TypedDict):
    id: int
    name: str | None  # None for a direct conversation
    stream: str | None  # its stream's name; None for a direct conversation
    resolved: bool  # its work is done; False for a direct conversation
    agents: list[str]  # its members: who joined a topic, or a direct's two

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
def clone(url: str, dest: str, rev: str = "") -> None:
    """Clone `url` into `dest` from the host's mirror, then detach at `rev`
    (empty: the remote's default branch). Blocking."""
def context(cwd: str) -> Context:
    """AGENTS.md files from `cwd` up to its repository root, and the skills
    found under it, as rho-context-config discovers them."""
def models() -> list[Runtime]:
    """The runtimes the host's accounts can run now."""

# -- agents ------------------------------------------------------------------

def create_agent(spec: Spec) -> str:
    """Log `AgentCreated` and start the agent's loop. Returns its id once the
    row is durable. Raises when the host is at its agent cap or the runtime
    is not available."""
def agents() -> dict[str, Spec]:
    """Every agent of this workset, by id."""
def set_spec(agent: str, spec: Spec) -> None:
    """Log `AgentChanged` with the whole new spec. A model change keeps the
    cache key and the Claude session."""
def input(agent: str, text: str, *, role: Role, message: int | None = None) -> None:
    """Give the agent an input: it waits in memory until a request can carry
    it, and that request's row holds it. This returns once the agent has it. A
    crash loses it unless it is a chat message, which is handed again. `role`
    says who speaks: "user" for what a person typed, "tool" for anything else.
    `message` names the chat message it hands over."""
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

# -- chat --------------------------------------------------------------------
#
# The host's chat: streams of topics of messages, and direct conversations of
# two agents. Every id is the position of the event that made it in the
# host's chat log. A message reaches its conversation's members and every
# agent it mentions, never its sender. A topic's members are the agents that
# joined it; neither posting nor a mention joins. A direct conversation's
# members are its two agents, and only they post in it. Messages from the
# user come only from the GUI, so Python cannot forge them. The user and
# agents may rename, move and resolve a topic, and move messages between
# topics.

def post(
    sender: str,
    topic: int,
    text: str,
    *,
    mentions: list[tuple[str, MentionKind]] | None = None,
) -> int:
    """Post in `topic` as `sender`, an agent of this workset; returns the
    message id once durable. A message that mentions "user" must pass the
    STE check, or this raises ValueError with the findings."""
def start_topic(
    sender: str,
    stream: str,
    name: str,
    text: str,
    *,
    mentions: list[tuple[str, MentionKind]] | None = None,
) -> tuple[int, int]:
    """Start a topic named `name` in the stream named `stream`, made if new,
    with `text` as its first message; returns `(topic, message)`."""
def direct(sender: str, other: str) -> int:
    """The direct conversation of two agents, started if new. They may be
    in different worksets."""
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
    host took the message. Raises when it refuses or gives no result."""
def external_contacts(agent: str) -> list[dict]:
    """The addresses `agent` accepted, each a dict of `address` and `name`."""
def join(topic: int, agent: str) -> None:
    """Make `agent` a member of the topic from now on. Not for a direct
    conversation."""
def leave(topic: int, agent: str) -> None:
    """`agent` stops being a member of the topic. Not for a direct
    conversation."""
def change_topic(agent: str, topic: int, stream: str, name: str, resolved: bool) -> None:
    """Give a topic its whole head, for `agent`: the stream of that name,
    made if new, its name, and whether it is resolved. Not for a direct
    conversation."""
def move_messages(
    agent: str,
    messages: list[int],
    to: int | None = None,
    *,
    stream: str | None = None,
    name: str | None = None,
) -> int:
    """Move messages for `agent` to topic `to`, or to a new topic `name` in
    `stream`; returns the topic they are in. A move delivers nothing again."""
def topic(id: int) -> TopicRecord | None: ...
def message(id: int) -> MessageRecord | None:
    """A message, in the topic it is in now: messages may be moved."""
def read(topic: int, last: int) -> list[MessageRecord]:
    """The conversation's `last` newest messages, oldest first."""
async def inbox(agent: str, after: int) -> list[MessageRecord]:
    """What `agent` receives after message `after`, waiting until there is
    some."""

# -- inside an agent's notebook ----------------------------------------------

def current_agent() -> str | None:
    """The agent whose notebook thread this is, or None on the workset
    thread."""
