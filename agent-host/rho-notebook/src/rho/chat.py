"""The host's chat: streams of topics of messages, and direct
conversations of two agents ([ARCH-agent-host]). Chat and agents meet only
here: a message reaches the members of its conversation and the agents it
mentions, never its sender. A topic's members are the agents that joined it.
A direct conversation's members are its two agents, and only they post in
it. The user is reached by a mention."""

import _rho

#: The address of the user.
USER = "user"
#: What a mention asks of the one it names.
KINDS = ("ask", "result", "fyi")
#: The prop that links an agent to the message it was started from, as
#: `chat:<message id>`.
ORIGIN = "origin"


def post(sender: str, topic: int, text: str, mentions: list[tuple[str, str]] = ()) -> int:
    """Post `text` in `topic`; returns the message id. `mentions` are
    `(address, kind)` pairs."""
    return _rho.post(sender, topic, text, mentions=list(mentions))


def start(sender: str, stream: str, name: str, text: str, mentions: list[tuple[str, str]] = ()) -> tuple[int, int]:
    """Start a topic named `name` in the stream named `stream`, made if it is
    new, with `text` as its first message; returns `(topic, message)`. No
    one joins it."""
    return _rho.start_topic(sender, stream, name, text, mentions=list(mentions))


def direct(a: str, b: str) -> int:
    """The direct conversation of agents `a` and `b`, started if new."""
    return _rho.direct(a, b)


def join(topic: int, agent: str) -> None:
    _rho.join(topic, agent)


def leave(topic: int, agent: str) -> None:
    _rho.leave(topic, agent)


def topic(id: int) -> dict | None:
    """The conversation's name, its stream's name, whether it is resolved,
    and its members. Name and stream are None for a direct conversation."""
    return _rho.topic(id)


def message(id: int) -> dict | None:
    return _rho.message(id)


def read(topic: int, last: int) -> list[dict]:
    """The conversation's `last` newest messages, oldest first."""
    return _rho.read(topic, last)


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


async def inbox(agent: str, after: int) -> list[dict]:
    """What `agent` receives after message `after`, waiting until there is
    some."""
    return await _rho.inbox(agent, after)
