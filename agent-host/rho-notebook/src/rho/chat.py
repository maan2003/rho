"""The host's chat: a forest of messages ([ARCH-agent-host]). Every message
hangs under a parent message or is a root; the messages under a message
are its thread, and a thread's id is its message's. Chat and agents meet
only here: a message reaches the members of the thread it is posted in and
of the threads above it, and the agents it mentions, never its sender. An
agent joins or leaves a thread; the nearest record up the chain decides.
The user is reached by a mention; another agent, by `mail`, which is not
chat: the host hands it to the agent and keeps no log."""

import _rho

#: The address of the user.
USER = "user"
#: What a mention asks of the one it names.
KINDS = ("ask", "result", "fyi")
#: The prop that links an agent to the message it was started from, as
#: `chat:<message id>`.
ORIGIN = "origin"


def post(sender: str, parent: int | None, text: str, mentions: list[tuple[str, str]] = ()) -> int:
    """Post `text` under `parent`, or as a root; returns the message id.
    `mentions` are `(address, kind)` pairs."""
    return _rho.post(sender, None if parent is None else int(parent), text, mentions=list(mentions))


def mail(sender: str, to: str, text: str) -> None:
    """Hand `text` to agent `to` as a message from `sender`, outside the
    chat."""
    from .prompt import label

    _rho.mail(sender, to, f"[message from {label(sender)}]\n{text}")


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
    """The root whose name is `name`, if there is one."""
    wanted = " ".join(name.split())
    for record in reversed(read(None, 1 << 31)):
        head = thread(record["id"])
        if head is not None and " ".join(head["name"].split()) == wanted:
            return record["id"]
    return None


def origin(id: int) -> str:
    """The `origin` prop of an agent started from message `id`."""
    return f"chat:{int(id)}"


def origin_message(props: dict) -> int | None:
    """The message an agent was started from, by its `origin` prop."""
    value = props.get(ORIGIN)
    if value is None:
        return None
    kind, colon, id = value.partition(":")
    if kind != "chat" or not colon or not id.isdigit():
        raise ValueError(f"origin is chat:<message id>, not {value!r}")
    return int(id)


def thread_of(record: dict) -> int:
    """The thread a message belongs to. A root is a stream, and a message
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
