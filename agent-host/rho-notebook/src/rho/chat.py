"""The host's chat as a forest of threads. The same record feeds the
notebook, the user's window, and search. The host decides delivery."""

import _rho

from . import agents

USER = "user"
ORIGIN = "origin"


def ident(value) -> int:
    """A thread or message id, as a positive integer or a `#12` label."""
    if type(value) is bool or not isinstance(value, (str, int)):
        raise ValueError(f"expected a message id, got {value!r}")
    digits = str(value).removeprefix("#")
    if not digits.isascii() or not digits.isdecimal() or not 0 < int(digits) < (1 << 64):
        raise ValueError(f"expected a message id, got {value!r}")
    return int(digits)


class Sent(int):
    """The posted message's id, awaitable without posting again."""

    def __await__(self):
        return self
        yield


def send_chat(sender, text, kind, *, thread) -> Sent:
    """Speak to the user in `thread`: a "message" pings, a "flash" does not."""
    _may_address_user(sender)
    if kind not in ("message", "flash"):
        raise ValueError("kind is 'message' or 'flash'")
    return Sent(_rho.send_chat(sender, ident(thread), str(text).strip(), kind))


def message_agent(sender, to, text) -> Sent:
    """`sender`'s words to agent `to`, kept under the sender's project."""
    if sender == to:
        raise ValueError("cannot message yourself")
    return Sent(_rho.message_agent(sender, to, str(text).strip()))


def _may_address_user(sender):
    props = agents.Agent(sender).props
    if props.get("preset") != "engineer":
        if (parent := props.get("parent")) is None:
            raise ValueError("only an Engineer can write to the user")
        from .prompt import label
        raise ValueError(
            f"write to your parent agent with agents.message(agent_id={label(parent)!r}, message=text)"
        )


def start_project(sender: str, name: str) -> int:
    """The project named `name`; a new one works where `sender` works."""
    return _rho.start_project(sender, str(name))


def join(thread: int, agent: str) -> None:
    _rho.join(int(thread), agent)


def leave(thread: int, agent: str) -> None:
    _rho.leave(int(thread), agent)


def reparent(agent: str, message: int, parent: int | None) -> None:
    """Put a message and its thread under `parent`, or among the roots,
    for `agent`."""
    _rho.reparent(agent, int(message), None if parent is None else int(parent))


def name(agent: str, thread: int, name: str) -> None:
    """Name a thread for `agent`; an empty name takes the name back."""
    _rho.name(agent, int(thread), name)


def thread(id: int) -> dict | None:
    """The thread of message `id`: its name, the ids and names of the
    threads above it and itself (`path`), and its members (`agents`)."""
    return _rho.thread(int(id))


def message(id: int) -> dict | None:
    return _rho.message(int(id))


def read(thread: int | None, last: int) -> list[dict]:
    """The `last` newest messages under `thread`, oldest first; the roots
    for `None`."""
    return _rho.read(None if thread is None else int(thread), int(last))


def root(name: str) -> int | None:
    """The project whose name is `name`, if there is one."""
    return _rho.project_named(name)


def origin(id: int) -> str:
    """The `origin` prop of an agent started from message `id`."""
    return f"chat:{int(id)}"  # persisted origin tag, shared with the window


def origin_message(props: dict) -> int | None:
    """The message an agent was started from, by its `origin` prop."""
    value = props.get(ORIGIN)
    if value is None:
        return None
    kind, colon, id = value.partition(":")
    if kind != "chat" or not colon or not id.isdigit():
        raise ValueError(f"invalid message origin: {value!r}")
    return int(id)


def thread_of(record: dict) -> int:
    """The thread a message belongs to. A root is a project, and a message
    right under a root starts a thread of its own; anything deeper is a
    reply in its parent's thread."""
    parent = record["parent"]
    if parent is None or message(parent)["parent"] is None:
        return record["id"]
    return parent


async def inbox(agent: str, after: int) -> list[dict]:
    """What `agent` receives after message `after`, waiting until there is
    some."""
    return await _rho.inbox(agent, after)
