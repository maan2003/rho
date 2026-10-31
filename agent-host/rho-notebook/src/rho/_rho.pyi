"""The Rust core of rho2, as Python sees it ([ARCH-rho2]).

This module is built into the workset process. The `rho` package wraps it;
policy code imports `rho`, not `_rho`. Every function runs in the workset
process and acts on its own workset only. Agent ids are their encoded text
(for example "x4cvbixbtifu"); mail addresses are "user" or an agent id.

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

class MailRecord(TypedDict):
    id: int
    at: int  # unix ms
    sender: str  # "user" or an agent id
    to: str
    re: int | None
    kind: Literal["ask", "result", "status", "fyi"] | None
    text: str

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
def input(agent: str, text: str, *, wake: Wake, mail: int | None = None) -> None:
    """Queue an input for the agent. Its loop logs the `Input` row; this
    returns once that row is durable. `mail` names the mail it hands over."""
def delivered(agent: str) -> int:
    """The newest mail id handed to the agent by `input`, or 0."""
def cancel(agent: str) -> None: ...
def compact(agent: str) -> None: ...

# -- mail --------------------------------------------------------------------

def send_mail(
    sender: str,
    to: str,
    text: str,
    *,
    kind: Literal["ask", "result", "status", "fyi"] | None = None,
    re: int | None = None,
) -> int:
    """Append a mail to the host's mail log; returns its id once durable.
    `sender` must be an agent of this workset. A mail to "user" needs a
    `kind`; the text must pass the STE check, or this raises ValueError
    with the findings."""
async def inbox(address: str, after: int) -> list[MailRecord]:
    """Mail to `address` newer than `after`, waiting until there is some."""

# -- inside an agent's notebook ----------------------------------------------

def current_agent() -> str | None:
    """The agent whose notebook thread this is, or None on the workset
    thread."""
