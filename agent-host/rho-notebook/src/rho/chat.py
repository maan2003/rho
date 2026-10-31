"""The host's chat: streams of topics of messages ([ARCH-agent-host]).
Every message is in a topic; it reaches the topic's agents and the agents
it mentions. The user is reached by a mention."""

import _rho

#: The address of the user.
USER = "user"
#: The stream every host has.
DEFAULT_STREAM = "rho"
#: What a mention asks of the one it names.
KINDS = ("ask", "result", "fyi")


def post(sender: str, topic: int, text: str, mentions: list[tuple[str, str]] = ()) -> int:
    """Post `text` in `topic`; returns the message id. `mentions` are
    `(address, kind)` pairs."""
    return _rho.post(sender, topic, text, mentions=list(mentions))


def start(sender: str, name: str, text: str, *, agents: list[str] = (), mentions: list[tuple[str, str]] = (), stream: str = DEFAULT_STREAM) -> int:
    """Start a topic with `text` as its first message; returns its id."""
    return _rho.start_topic(sender, stream, name, text, agents=list(agents), mentions=list(mentions))


def join(topic: int, agent: str) -> None:
    _rho.join(topic, agent)


def topic(id: int) -> dict | None:
    """The topic's name, its stream's name and its agents."""
    return _rho.topic(id)


async def inbox(agent: str, after: int) -> list[dict]:
    """What `agent` receives after message `after`, waiting until there is
    some."""
    return await _rho.inbox(agent, after)
