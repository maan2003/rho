"""Mail between the user and agents, kept by the host ([ARCH-rho2]).
An address is "user" or an agent id."""

import _rho

USER = "user"


def send(sender: str, to: str, text: str, kind: str | None = None, thread: int | None = None) -> int:
    """Append a mail; returns its id. `sender` is an agent of this workset.
    Mail to the user needs a `kind`: "ask", "result", "status" or "fyi".
    `thread` is the id of a thread's first mail; None starts a thread."""
    return _rho.send_mail(sender, to, text, kind=kind, thread=thread)


def thread_with(a: str, b: str) -> int | None:
    """The thread of the newest mail between `a` and `b`, either way: where
    their conversation goes on. None when they never wrote."""
    newest = _rho.newest_mail_between(a, b)
    return None if newest is None else newest["thread"]


async def inbox(address: str, after: int) -> list[dict]:
    """Mail to `address` newer than `after`, waiting until there is some."""
    return await _rho.inbox(address, after)
