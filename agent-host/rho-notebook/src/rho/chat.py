"""The host's chat: streams of topics of messages, direct conversations of
two agents, and peer conversations of an agent and an address on another
host ([ARCH-agent-host]). Chat and agents meet only here: a message reaches
the members of its conversation and the agents it mentions, never its
sender. A topic's members are the agents that joined it. A direct
conversation's members are its two agents, and only they post in it. A peer
conversation is the same with one agent: the host sends what it posts to
the address it accepted, and what that address sends arrives in it. The
user is reached by a mention."""

import _rho

#: The address of the user.
USER = "user"
#: The sender of what the other end of a peer conversation sent.
PEER = "peer"
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


def address(agent: str) -> str:
    """The C2C address other hosts reach `agent` at."""
    return _rho.peer_address(agent)


def accept(agent: str, address: str, name: str) -> int:
    """`agent` accepts the C2C `address` under `name`; returns their peer
    conversation. The host then carries messages between the two."""
    return _rho.peer_accept(agent, address, name)


def peer(agent: str, address: str) -> int:
    """The peer conversation of `agent` and an address it accepted."""
    return _rho.peer_topic(agent, address)


def contacts(agent: str) -> list[dict]:
    """The addresses `agent` has accepted."""
    return _rho.peers(agent)


def join(topic: int, agent: str) -> None:
    _rho.join(topic, agent)


def leave(topic: int, agent: str) -> None:
    _rho.leave(topic, agent)


def change(agent: str, topic: int, *, stream: str | None = None, name: str | None = None, resolved: bool | None = None) -> None:
    """Change a topic for `agent`: what is not given stays as it is."""
    head = _rho.topic(topic)
    if head is None:
        raise ValueError(f"no topic {topic}")
    if head["stream"] is None:
        raise ValueError("a direct conversation is not renamed, moved or resolved")
    _rho.change_topic(
        agent,
        topic,
        head["stream"] if stream is None else stream,
        head["name"] if name is None else name,
        head["resolved"] if resolved is None else resolved,
    )


def move(agent: str, messages: list[int], to: int | None = None, *, stream: str | None = None, name: str | None = None) -> int:
    """Move messages for `agent` to topic `to`, or to a new topic `name` in
    `stream`; returns the topic they are in."""
    if to is None:
        return _rho.move_messages(agent, list(messages), stream=stream, name=name)
    return _rho.move_messages(agent, list(messages), to)


def topic(id: int) -> dict | None:
    """The conversation's name, its stream's name, whether it is resolved,
    its members, and its peer. Name and stream are None outside a stream;
    the peer is None but for a peer conversation."""
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
