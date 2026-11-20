"""Host mail as a forest of threads. The same record feeds the notebook,
the user's mail window, and search. The host decides delivery."""

import _rho

from . import agents

USER = "user"
KINDS = ("ask", "result", "fyi")
ORIGIN = "origin"


def ident(value) -> int:
    """A thread or mail id, as a positive integer or a `#12` label."""
    if type(value) is bool or not isinstance(value, (str, int)):
        raise ValueError(f"expected a mail id, got {value!r}")
    digits = str(value).removeprefix("#")
    if not digits.isascii() or not digits.isdecimal() or not 0 < int(digits) < (1 << 64):
        raise ValueError(f"expected a mail id, got {value!r}")
    return int(digits)


def _local_address(sender, to):
    name, at, host = to.rpartition("@")
    if at:
        if not name or not host:
            raise ValueError("an address is name@host")
        # Window aliases live on the user's device. The notebook can
        # verify @local or this endpoint, never route by a remote alias.
        if host != "local" and (len(host) != 64 or host != _rho.external_address(sender).partition("/")[0]):
            raise ValueError(f"not this host: {host!r}; use @local")
        return name
    return to


class SentMail(int):
    """The new mail's id, with any follow-up suggestion in `notice`."""

    def __new__(cls, id: int, notice: str | None = None):
        self = super().__new__(cls, id)
        self.notice = notice
        return self

    def __await__(self):
        return self
        yield


def _send(sender, parent, text, **kwargs) -> SentMail:
    id, unanswered = _rho.send_mail(sender, parent, text, **kwargs)
    if unanswered is None:
        return SentMail(id)
    prior, author = unanswered
    edit = f"use `mail.edit({prior}, ...)`" if author == sender else "ask its writer to use `mail.edit`"
    return SentMail(id, f"Mail #{prior} has no user reply. If it is still live, {edit} for updates.")


def send(sender, to, text, *, thread=None, reply_to=None, kind=None) -> SentMail:
    """Send to a local project, #thread, agent handle, or `user`.
    `reply_to=#id` identifies the exact mail answered. To address its
    writer use `to=None`. Mail to `user` needs kind and a thread or reply.
    """
    text = str(text).strip()
    if not text:
        raise ValueError("mail needs text")
    reply = None
    if reply_to is not None:
        reply_to = ident(reply_to)
        reply = message(reply_to)
        if reply is None:
            raise ValueError(f"no mail #{reply_to}")
    if to is None:
        if reply is None:
            raise ValueError("mail needs an address or reply_to")
        writer = reply["sender"]
        if writer == "system":
            raise ValueError("address the thread to answer system mail")
        if writer == USER:
            to = USER
        else:
            to = _rho.agent_handle(writer)
            if to is None:
                raise ValueError(f"no agent {writer} on this host")

    if isinstance(to, str):
        to = _local_address(sender, to.strip())
    if to == USER:
        if kind not in KINDS:
            raise ValueError('mail to the user needs kind: "ask", "result" or "fyi"')
        props = agents.Agent(sender).props
        if props.get("preset") != "engineer":
            if (parent := props.get("parent")) is None:
                raise ValueError("only an Engineer can mail the user")
            from .prompt import label
            raise ValueError(f"mail your parent agent {label(parent)} instead")
        if thread is None:
            if reply is None:
                raise ValueError("mail to the user needs thread or reply_to")
            thread = thread_of(reply)
        return _send(sender, ident(thread), text, to=USER, kind=kind, reply_to=reply_to)

    if thread is not None or kind is not None:
        raise ValueError("only mail to the user takes thread or kind")
    if isinstance(to, str) and to.startswith(("eng-", "adv-")):
        target = agents.resolve(to)
        if sender == target:
            raise ValueError("cannot mail yourself")
        return _send(sender, None, text, to=target, reply_to=reply_to)
    if isinstance(to, int) or isinstance(to, str) and to.startswith("#"):
        parent = ident(to)
    elif isinstance(to, str):
        parent = root(to)
        if parent is None:
            raise ValueError(f"no project {to!r} on this host")
    else:
        raise ValueError(f"unknown mail address {to!r}")
    return _send(sender, parent, text, reply_to=reply_to)


def make_project(sender: str, name: str) -> int:
    return _rho.send_mail(sender, None, name)[0]


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
    return _rho.project_named(name)


def origin(id: int) -> str:
    """The `origin` prop of an agent started from message `id`."""
    return f"chat:{int(id)}"  # origin tag shared with the window until its protocol rename


def origin_message(props: dict) -> int | None:
    """The message an agent was started from, by its `origin` prop."""
    value = props.get(ORIGIN)
    if value is None:
        return None
    kind, colon, id = value.partition(":")
    if kind != "chat" or not colon or not id.isdigit():
        raise ValueError(f"invalid mail origin: {value!r}")
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
