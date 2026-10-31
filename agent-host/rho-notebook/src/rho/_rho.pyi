"""The Rust core of rho, as Python sees it ([ARCH-agent-host]).

This module is built into the workset process. The `rho` package wraps it;
policy code imports `rho`, not `_rho`. Every function runs in the workset
process and acts on its own workset only. Agent ids are their encoded text
(for example "x4cvbixbtifu"); a chat address is "user" or an agent id.

Blocking calls release the GIL. Calls marked async return awaitables that
run on the asyncio loop of the caller's thread.
"""

from typing import Literal, TypedDict

Wake = Literal["person", "agent", "quiet"]

class NativeRuntime(TypedDict):
    kind: Literal["native"]
    model: Literal["sol", "terra", "astra"]
    effort: Literal["low", "medium", "high", "xhigh"]
    fast: bool

class ClaudeRuntime(TypedDict):
    kind: Literal["claude"]
    model: Literal["fable", "opus", "sonnet"]
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
    name: str
    stream: str  # its stream's name
    agents: list[str]  # the agents that receive its messages

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
def input(agent: str, text: str, *, wake: Wake, message: int | None = None) -> None:
    """Queue an input for the agent. Its loop logs the `Input` row; this
    returns once that row is durable. `message` names the chat message it
    hands over."""
def delivered(agent: str) -> int:
    """The newest message id handed to the agent by `input`, or 0."""
def cancel(agent: str) -> None: ...
def compact(agent: str) -> None: ...
def rewind(agent: str, turns: int) -> None:
    """Branch the agent's history before its `turns`-th last input from the
    user. The notebook stays."""
def new_cache_key(agent: str) -> None:
    """Give a native agent a fresh prompt cache key (an `AgentChanged`)."""

# -- chat --------------------------------------------------------------------
#
# The host's chat: streams of topics of messages. Every id is the position of
# the event that made it in the host's chat log. A message reaches every
# agent of its topic and every agent it mentions, never its sender; posting
# makes the sender one of the topic's agents. Messages from the user come
# only from the GUI, so Python cannot forge them.

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
    agents: list[str] = [],
    mentions: list[tuple[str, MentionKind]] | None = None,
) -> int:
    """Start a topic named `name` in the stream named `stream`, with `text`
    as its first message and `agents` receiving it; returns the topic id."""
def join(topic: int, agent: str) -> None:
    """Make `agent` one of the topic's agents from now on."""
def topic(id: int) -> TopicRecord | None: ...
async def inbox(agent: str, after: int) -> list[MessageRecord]:
    """What `agent` receives after message `after`, waiting until there is
    some."""

# -- inside an agent's notebook ----------------------------------------------

def current_agent() -> str | None:
    """The agent whose notebook thread this is, or None on the workset
    thread."""
